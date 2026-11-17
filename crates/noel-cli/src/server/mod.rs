//! Mode 2 (spec §5–§8): a long-running `noel server` that holds sessions, one
//! `noel worker` per session that runs it, and clients that reach them, all
//! over Chord link protocol 2 (`noel-chord`).
//!
//! - **The server** ([`serve`]) is the user's one host, `server`, on an
//!   owner-only Unix socket in the runtime directory. It serves
//!   `noel.sessions` and `noel.models`, routes `Session({id})` addresses to its workers,
//!   spawns, watches and adopts them, and resumes a run a worker crash
//!   interrupted.
//! - **A worker** ([`worker`]) is the host at `Session({id})`: the session's only
//!   writer, holding its journal lock. It serves `noel.chat` with the view
//!   of the conversation ([`view`]) and drives runs with the session
//!   controller. It outlives its server and waits to be adopted.
//! - **The client** ([`client`]) is line-mode `noel client`.
//! - **The bridge** ([`bridge`]) is `noel bridge`: the server's socket on
//!   stdin and stdout, so a client on another machine reaches the server
//!   with `ssh <host> noel bridge` (`--remote`).
//!
//! The owner reaches the server two ways: on this machine, by the socket's
//! peer uid (`via: "device"`), and through the bridge, as the user ssh let
//! in (`via: "tunnel"`). There is no relay, pairing or guest yet.

pub(crate) mod accounts;
pub(crate) mod bridge;
pub(crate) mod client;
mod djs_plugins;
pub(crate) mod plugins;
pub(crate) mod protocol;
pub(crate) mod remote;
pub(crate) mod serve;
mod view;
pub(crate) mod worker;

use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use std::sync::Arc;

use noel_chord::v2::Contract;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

/// `EX_TEMPFAIL`: another server is running, or the session's journal is
/// locked by another process.
pub(crate) const EXIT_LOCKED: i32 = 75;

/// Overrides where the server's socket, lock and worker sockets live.
pub(crate) const ENV_RUNTIME_DIR: &str = "NOEL_RUNTIME_DIR";

/// The upgrade request header a bridge sets to `tunnel`; without it a link
/// on the socket is the device's.
pub(crate) const VIA_HEADER: &str = "Noel-Via";

/// The worker's `ready` and `fatal` lines on stdout, and its `ready` line
/// on a new socket connection before the framed link, are
/// `{"chord2control": message}`.
pub(crate) const CONTROL_KEY: &str = "chord2control";

/// Default for `server.idleExitMs`.
pub(crate) const DEFAULT_SERVER_IDLE_MS: u64 = 10 * 60 * 1000;
/// Default for `server.workerIdleExitMs`.
pub(crate) const DEFAULT_WORKER_IDLE_MS: u64 = 60 * 1000;

// The contracts live in noel-client, which every front end shares.
pub(crate) fn chat_contract() -> Arc<Contract> {
    noel_client::contracts::chat()
}

pub(crate) fn sessions_contract() -> Arc<Contract> {
    noel_client::contracts::sessions()
}

pub(crate) fn models_contract() -> Arc<Contract> {
    noel_client::contracts::models()
}

/// Runs `noel server`, `noel worker`, `noel client` or `noel bridge` when
/// the command line names one, and returns the exit status; `None` for any
/// other command.
pub(crate) async fn try_run(args: &[String]) -> Option<i32> {
    use clap::Parser;
    // Leading global flags (`noel --approve mcp list`) must still reach
    // the subcommand dispatch, or the run falls through to a session;
    // skip them and hand both halves to the subcommand parser, whose
    // globals accept any position clap allows.
    let mut start = 1;
    while args.get(start).is_some_and(|arg| arg.starts_with('-')) {
        start += 1;
    }
    let command = args.get(start)?.as_str();
    let rest = std::iter::once(format!("noel {command}"))
        .chain(args[1..start].iter().cloned())
        .chain(args[start + 1..].iter().cloned());
    Some(match command {
        "server" => match serve::ServerArgs::try_parse_from(rest) {
            Ok(parsed) => serve::run(parsed).await,
            Err(error) => usage(error),
        },
        "worker" => match worker::WorkerArgs::try_parse_from(rest) {
            Ok(parsed) => worker::run(parsed).await,
            Err(error) => usage(error),
        },
        "client" => match client::ClientArgs::try_parse_from(rest) {
            Ok(parsed) => client::run(parsed).await,
            Err(error) => usage(error),
        },
        "bridge" => match bridge::BridgeArgs::try_parse_from(rest) {
            Ok(parsed) => bridge::run(parsed).await,
            Err(error) => usage(error),
        },
        "app" => app(&args[2..]),
        _ => return None,
    })
}

fn usage(error: clap::Error) -> i32 {
    let code = error.exit_code();
    let _ = error.print();
    code
}

// ─── the runtime directory ──────────────────────────────────────────────────

/// Where the server's socket and lock and the workers' sockets live:
/// `NOEL_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/noel`, else `/tmp/noel-<uid>`.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeDir {
    root: PathBuf,
}

/// This user's home directory as the password database names it.
fn passwd_home() -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: getpwuid_r writes only into `entry` and `buffer`, whose
    // lengths it is given; `found` points at `entry` or is null.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        );
        if status != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(entry.pw_dir);
        Some(std::ffi::OsStr::from_bytes(dir.to_bytes()).to_os_string())
    }
}

/// `-<hash>` of NOEL_DIR, NOEL_SESSION_DIR and a HOME other than the
/// user's own (which moves the default agent and session dirs) when any is
/// set, so each has its own server; empty for the defaults.
fn scope_suffix() -> String {
    use noel_config::env;
    use sha2::{Digest, Sha256};
    let dir = env::non_empty(env::AGENT_DIR);
    let sessions = env::non_empty(env::SESSION_DIR);
    let own_home = passwd_home();
    let home = env::non_empty("HOME").filter(|home| Some(home) != own_home.as_ref());
    if dir.is_none() && sessions.is_none() && home.is_none() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    for part in [dir, sessions, home] {
        hasher.update(part.unwrap_or_default().as_encoded_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex = digest[..8].iter().map(|byte| format!("{byte:02x}"));
    format!("-{}", hex.collect::<String>())
}

impl RuntimeDir {
    pub fn locate() -> PathBuf {
        if let Some(dir) = std::env::var_os(ENV_RUNTIME_DIR).filter(|dir| !dir.is_empty()) {
            return PathBuf::from(dir);
        }
        // A server serves one agent dir and one session dir: a `noel` with
        // other ones (NOEL_DIR, NOEL_SESSION_DIR) has its own server, in a
        // directory named after them next to the default one.
        let scope = scope_suffix();
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir).join(format!("noel{scope}")),
            None => PathBuf::from(format!("/tmp/noel-{}{scope}", own_uid())),
        }
    }

    /// The directory, created with mode 0700 when missing (its parent must
    /// exist). An existing one must be a directory, not a link, owned by
    /// this user with permissions 0700, or nothing runs in it.
    pub fn ensure(root: PathBuf) -> Result<Self> {
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder
                    .create(&root)
                    .map_err(|error| anyhow!("create {}: {error}", root.display()))?;
                // A setgid parent passes its bit on; the directory is 0700.
                set_mode(&root, 0o700)?;
            }
            Err(error) => return Err(anyhow!("{}: {error}", root.display())),
            Ok(_) => {}
        }
        Self::check(root)
    }

    /// An existing directory that is safe to use (see [`RuntimeDir::ensure`]).
    pub fn check(root: PathBuf) -> Result<Self> {
        let metadata =
            fs::symlink_metadata(&root).map_err(|error| anyhow!("{}: {error}", root.display()))?;
        if !metadata.is_dir() {
            return Err(anyhow!(
                "{} is not a directory; refusing to use it",
                root.display()
            ));
        }
        #[cfg(unix)]
        {
            if metadata.uid() != own_uid() {
                return Err(anyhow!(
                    "{} is owned by uid {}, not {}; refusing to use it",
                    root.display(),
                    metadata.uid(),
                    own_uid()
                ));
            }
            // Only the permissions decide who may enter: a setgid or sticky
            // bit on a 0700 directory of this user lets no one else in.
            let mode = metadata.permissions().mode() & 0o7777;
            if mode & 0o777 != 0o700 {
                return Err(anyhow!(
                    "{} has mode {mode:o}, not 700; refusing to use it",
                    root.display()
                ));
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn server_socket(&self) -> PathBuf {
        self.root.join("server.sock")
    }

    pub fn server_lock(&self) -> PathBuf {
        self.root.join("server.lock")
    }

    pub fn server_record(&self) -> PathBuf {
        self.root.join("server.json")
    }

    pub fn server_log(&self) -> PathBuf {
        self.root.join("server.log")
    }

    pub fn workers(&self) -> PathBuf {
        self.root.join("workers")
    }

    pub fn worker_socket(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.sock"))
    }

    pub fn worker_record(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.json"))
    }

    /// The flags of the `noel` that opened the session, for its next
    /// worker (docs/client-tui-plan.md).
    pub fn worker_args(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.args.json"))
    }

    /// The journal of a session kept outside the session directory
    /// (`noel --session <path>`): its path, one line.
    pub fn worker_journal(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.journal"))
    }

    /// The flags the session's running worker started with.
    pub fn worker_applied_args(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.applied.json"))
    }

    pub fn worker_log(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.log"))
    }

    /// `workers/`, created with mode 0700 when missing.
    pub fn ensure_workers(&self) -> Result<PathBuf> {
        let dir = self.workers();
        if !dir.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(&dir)
                .map_err(|error| anyhow!("create {}: {error}", dir.display()))?;
        }
        Ok(dir)
    }
}

pub(crate) fn own_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Removes a socket left at `path` by a process that is gone, only when it
/// is a socket this user owns. Callers hold the lock that makes them its
/// only user.
pub(crate) fn remove_stale_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow!("{}: {error}", path.display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() || metadata.uid() != own_uid() {
            return Err(anyhow!(
                "{} exists and is not a socket of this user; refusing to replace it",
                path.display()
            ));
        }
    }
    fs::remove_file(path).map_err(|error| anyhow!("remove {}: {error}", path.display()))
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| anyhow!("chmod {}: {error}", path.display()))?;
    Ok(())
}

/// Writes `value` to `path` (mode 0600) by replacing it.
pub(crate) fn write_record(path: &Path, value: &Value) -> Result<()> {
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    fs::write(&temporary, format!("{value}\n"))
        .map_err(|error| anyhow!("write {}: {error}", temporary.display()))?;
    set_mode(&temporary, 0o600)?;
    fs::rename(&temporary, path).map_err(|error| anyhow!("write {}: {error}", path.display()))
}

/// The other end of a Unix socket, checked before a byte is read: its uid
/// must be this user's. Returns its pid when the platform reports one.
pub(crate) fn peer_check(stream: &UnixStream) -> std::result::Result<Option<u32>, String> {
    let credentials = stream
        .peer_cred()
        .map_err(|error| format!("no peer credentials: {error}"))?;
    if credentials.uid() != own_uid() {
        return Err(format!("uid {} is not the owner", credentials.uid()));
    }
    Ok(credentials.pid().and_then(|pid| u32::try_from(pid).ok()))
}

/// Names this binary: its version, and the identity of the executable file
/// the process runs (device, inode, size and modification time). A server
/// adopts only workers of its own build.
pub(crate) fn build_id() -> String {
    let metadata = fs::metadata("/proc/self/exe")
        .or_else(|_| std::env::current_exe().and_then(fs::metadata))
        .ok();
    let identity = metadata
        .map(|metadata| {
            #[cfg(unix)]
            {
                format!(
                    "{:x}.{:x}.{:x}.{:x}",
                    metadata.dev(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime_nsec() as u64 ^ ((metadata.mtime() as u64) << 20)
                )
            }
            #[cfg(not(unix))]
            {
                format!("{:x}", metadata.len())
            }
        })
        .unwrap_or_else(|| "unknown".to_owned());
    format!("{}+{identity}", env!("CARGO_PKG_VERSION"))
}

/// The program a server runs for its workers and a client for its server:
/// this process's own executable, even when it was replaced on disk.
pub(crate) fn own_exe() -> PathBuf {
    let proc_exe = PathBuf::from("/proc/self/exe");
    if proc_exe.exists() {
        return proc_exe;
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("noel"))
}

/// A session id the server routes: what `--session-id` accepts, so it is a
/// file name and never a path.
pub(crate) fn valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !id.contains("..")
}

/// The id of the session a `create` or `fork` with `request_id` makes, so a
/// repeat finds it instead of making another (spec §3.1's `requestId`).
pub(crate) fn derived_session_id(method: &str, request_id: &str) -> String {
    let digest = Sha256::digest(format!("noel.sessions.{method}\0{request_id}").as_bytes());
    let hex = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("s-{hex}")
}

/// The owner's principal: `via` is `device` for the owner's processes on
/// this machine, `tunnel` for a client through `noel bridge`.
pub(crate) fn owner(via: &str) -> Value {
    json!({"kind": "owner", "via": via})
}

/// The principal of a worker's own calls.
pub(crate) fn worker_principal(session: &str, generation: u64) -> Value {
    json!({"kind": "worker", "session": session, "generation": generation})
}

/// The `noel.chat` variant `principal` gets on `session`: the owner and a
/// full guest of the session participate, a viewing guest views, anyone
/// else gets nothing (spec §6, §7.2).
pub(crate) fn chat_variant(principal: &Value, session: &str) -> Option<&'static str> {
    match principal.get("kind").and_then(Value::as_str) {
        // The server itself (the worker's parent link), which renames and
        // deletes a session it holds through it (`noel.sessions`).
        Some("owner") | Some("server") => Some("participant"),
        Some("guest") if principal.get("session").and_then(Value::as_str) == Some(session) => {
            match principal.get("mode").and_then(Value::as_str) {
                Some("full") => Some("participant"),
                Some("view") => Some("viewer"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Errors as the server and its workers give them (Chord v2): Chord's own
/// codes for what Chord means, plugin codes for noel's refusals (`busy`,
/// `session`, `cwd`, ...), which v2's closed reason table cannot carry.
pub(crate) mod errors {
    use noel_chord::v2::{ChordError, Code, WireError};
    use serde_json::json;

    pub(crate) fn unavailable(message: impl Into<String>) -> ChordError {
        ChordError::new(Code::Unavailable, message)
    }

    /// `unavailable` with `reason` when v2 allows it (`locked`, `busy`,
    /// ...), else with the reason in its details.
    pub(crate) fn unavailable_because(reason: &str, message: impl Into<String>) -> ChordError {
        if Code::Unavailable.reasons().contains(&reason) {
            ChordError::with_reason(Code::Unavailable, reason, message)
        } else {
            unavailable(message).details(json!({ "reason": reason }))
        }
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> WireError {
        ChordError::new(Code::Forbidden, message).into()
    }

    /// A refusal of noel's: the plugin error `code`.
    pub(crate) fn refused(code: &str, message: impl Into<String>) -> WireError {
        WireError::plugin(code, message)
    }

    /// A session the server has no journal for: `unavailable` with
    /// `details.missing: "session"` (a route's failure is a Chord error).
    pub(crate) fn no_session(id: &str) -> ChordError {
        unavailable(format!("no session {id}")).details(json!({ "missing": "session" }))
    }

    /// Whether `error` says a session does not exist.
    pub(crate) fn is_no_session(error: &WireError) -> bool {
        match error {
            WireError::Chord(error) => {
                error
                    .details
                    .as_ref()
                    .and_then(|details| details.get("missing"))
                    == Some(&json!("session"))
            }
            WireError::Plugin(error) => error.code == "session",
        }
    }
}

/// A control line: `{"chord2control": message}`.
pub(crate) fn control_line(message: &Value) -> String {
    let mut line = json!({ CONTROL_KEY: message }).to_string();
    line.push('\n');
    line
}

/// The message of a control line, if `line` is one.
pub(crate) fn parse_control(line: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(line.trim()).ok()?;
    value.as_object_mut()?.remove(CONTROL_KEY)
}

/// `noel app [--remote <dest>] [--session <id>]`: runs the desktop app
/// (`noel-app`, apps/desktop), beside this program or on the PATH, on this
/// program's server.
fn app(args: &[String]) -> i32 {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("noel-app")))
        .filter(|path| path.is_file());
    let program = beside.unwrap_or_else(|| PathBuf::from("noel-app"));
    let mut command = std::process::Command::new(&program);
    // This program's resolved path: `/proc/self/exe` would name the app in
    // the app's own process.
    if !args.iter().any(|arg| arg == "--remote" || arg == "--noel")
        && let Ok(noel) = std::env::current_exe()
    {
        command.arg("--noel").arg(noel);
    }
    match command.args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!(
                "noel app: {}: {error} (build it with `make app`)",
                program.display()
            );
            127
        }
    }
}

/// A random id for a `requestId`.
pub(crate) fn new_request_id() -> String {
    noel_client::new_request_id()
}

/// Milliseconds since the epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nrt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_runtime_directory_is_made_private_and_a_shared_one_is_refused() {
        let dir = temp("rd");
        let made = RuntimeDir::ensure(dir.clone()).unwrap();
        let mode = fs::metadata(made.root()).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
        // A directory others may enter is refused, and left as it is.
        set_mode(&dir, 0o755).unwrap();
        let error = RuntimeDir::ensure(dir.clone()).unwrap_err().to_string();
        assert!(error.contains("mode 755"), "{error}");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        // A link to a private directory is refused too.
        set_mode(&dir, 0o700).unwrap();
        let link = temp("rl");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(RuntimeDir::check(link.clone()).is_err());
        fs::remove_file(link).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_setgid_parent_leaves_the_runtime_directory_usable() {
        let parent = temp("sg");
        fs::create_dir(&parent).unwrap();
        set_mode(&parent, 0o2755).unwrap();
        // Only when this system lets a user set the bit on their own directory.
        if fs::metadata(&parent).unwrap().permissions().mode() & 0o2000 != 0 {
            let dir = parent.join("noel");
            let made = RuntimeDir::ensure(dir.clone()).unwrap();
            let mode = fs::metadata(made.root()).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o700);
            // One made earlier with the inherited bit is used as it is.
            set_mode(&dir, 0o2700).unwrap();
            RuntimeDir::ensure(dir.clone()).unwrap();
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
                0o2700
            );
            // Group permissions are still refused.
            set_mode(&dir, 0o2750).unwrap();
            let error = RuntimeDir::ensure(dir).unwrap_err().to_string();
            assert!(error.contains("mode 2750"), "{error}");
        }
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn only_a_stale_socket_of_this_user_is_removed() {
        let dir = temp("st");
        RuntimeDir::ensure(dir.clone()).unwrap();
        let file = dir.join("server.sock");
        fs::write(&file, "not a socket").unwrap();
        assert!(remove_stale_socket(&file).is_err());
        assert!(file.exists());
        fs::remove_file(&file).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&file).unwrap();
        drop(listener);
        remove_stale_socket(&file).unwrap();
        assert!(!file.exists());
        remove_stale_socket(&file).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_ids_are_file_names_and_derived_ids_are_stable() {
        for id in ["abc", "1760-123-0", "s-0a1b", "a.b_c-d"] {
            assert!(valid_session_id(id), "{id}");
        }
        for id in ["", "../x", "a/b", ".hidden", "x..y", "-a", "a-", "a b"] {
            assert!(!valid_session_id(id), "{id}");
        }
        let id = derived_session_id("create", "r1");
        assert_eq!(id, derived_session_id("create", "r1"));
        assert_ne!(id, derived_session_id("create", "r2"));
        assert_ne!(id, derived_session_id("fork", "r1"));
        assert!(valid_session_id(&id) && id.len() == 26, "{id}");
    }

    #[test]
    fn principals_get_their_variant() {
        assert_eq!(chat_variant(&owner("device"), "s1"), Some("participant"));
        assert_eq!(chat_variant(&owner("tunnel"), "s1"), Some("participant"));
        let guest = |session: &str, mode: &str| json!({"kind": "guest", "id": "g", "name": "G", "session": session, "mode": mode, "revision": 2});
        assert_eq!(chat_variant(&guest("s1", "view"), "s1"), Some("viewer"));
        assert_eq!(
            chat_variant(&guest("s1", "full"), "s1"),
            Some("participant")
        );
        assert_eq!(chat_variant(&guest("s2", "full"), "s1"), None);
        assert_eq!(chat_variant(&worker_principal("s1", 1), "s1"), None);
        assert_eq!(chat_variant(&json!({"host": "server"}), "s1"), None);
    }

    #[test]
    fn the_contracts_parse_and_have_their_variants() {
        let chat = chat_contract();
        let declaration = &chat.declaration;
        assert_eq!(
            (declaration.name.as_str(), declaration.version),
            ("noel.chat", 1)
        );
        assert!(declaration.variants.contains_key("viewer"));
        assert!(declaration.variants.contains_key("participant"));
        assert_eq!(sessions_contract().declaration.name, "noel.sessions");
        assert_eq!(models_contract().declaration.name, "noel.models");
    }

    #[test]
    fn control_lines_round_trip() {
        let line = control_line(&json!({"type": "ready", "pid": 7}));
        assert_eq!(line, "{\"chord2control\":{\"pid\":7,\"type\":\"ready\"}}\n");
        assert_eq!(
            parse_control(&line),
            Some(json!({"type": "ready", "pid": 7}))
        );
        assert_eq!(parse_control("{\"chord2\":{}}"), None);
        assert_eq!(parse_control("ready"), None);
    }
}
